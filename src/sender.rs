//! Transaction orchestrator. See [`Sender`].

use crate::{chain, chain::ChainClient, Error, GasPriceManager, Message, NonceManager, PriorityQueue};
use alloy::consensus::TxEip1559;
use alloy::network::Ethereum;
use alloy::primitives::{TxHash, TxKind};
use alloy::providers::PendingTransactionBuilder;
use alloy::transports::RpcError::ErrorResp;
use alloy::{
    primitives::{Address, B256},
    rpc::types::TransactionReceipt,
};
use futures::future::join;
use log::{debug, error, info};
use std::time::Duration;
use std::{collections::HashMap, sync::Arc, time::Instant};
use tokio::sync::{Mutex, Semaphore};

/// Maximum number of transactions that may be in-flight simultaneously.
const MAX_IN_FLIGHT_TRANSACTIONS: usize = 16;

/// Maximum number of times a stuck transaction may be replaced with a higher fee.
const MAX_REPLACEMENTS: u32 = 3;

/// Percentage by which the priority fee is increased on each replacement.
const GAS_PRICE_INCREASE_PERCENT: u8 = 20;

/// Seconds to wait for a transaction to confirm before treating it as dropped.
const TX_TIMEOUT: u64 = 3;

/// JSON-RPC error code returned when the sender has insufficient funds.
const TX_FAILURE_INSUFFICIENT_FUNDS: i64 = -32003;

type PendingMap = HashMap<TxHash, PendingTransaction>;
type SharedPendingMap = Arc<Mutex<PendingMap>>;
type SharedQueue = Arc<Mutex<PriorityQueue>>;

#[derive(Clone)]
struct PendingTransaction {
    msg: Message,
    created_at: Instant,
    replacement_count: u32,
    priority_fee: u128,
    nonce: u64,
}

/// An orchestrator that manages concurrent EIP-1559 transaction submission.
///
/// [`Sender`] owns the priority queue, the in-flight pending map, and all
/// shared resources (nonce manager, gas-price manager, semaphore).
/// Callers add [`Message`] values via [`add_message`] and then drive
/// processing by calling [`run`], which blocks until an unrecoverable error
/// occurs.
///
/// Up to 16 messages are processed concurrently. Each message is handled in
/// its own spawned task. A semaphore prevents the queue from being consumed
/// faster than the concurrency limit allows.
///
/// [`add_message`]: Self::add_message
/// [`run`]: Self::run
#[derive(Clone)]
pub struct Sender {
    chain: Arc<dyn ChainClient>,
    nonce_manager: Arc<NonceManager>,
    gas_manager: Arc<GasPriceManager>,

    queue: SharedQueue,
    pending: SharedPendingMap,
    max_in_flight: Arc<Semaphore>,
}

impl Sender {
    /// Creates a new [`Sender`] connected to `chain` and bound to `address`.
    ///
    /// `address` is used to fetch the initial nonce and to sync nonces on
    /// each new block. It must correspond to the signing key held by `chain`.
    pub async fn new(chain: Arc<dyn ChainClient>, address: Address) -> Result<Self, Error> {
        Ok(Self {
            chain: chain.clone(),
            nonce_manager: Arc::new(NonceManager::new(chain.clone(), address).await?),
            gas_manager: Arc::new(GasPriceManager::new()),

            queue: Arc::new(Mutex::new(PriorityQueue::new())),
            pending: Arc::new(Mutex::new(HashMap::new())),
            max_in_flight: Arc::new(Semaphore::new(MAX_IN_FLIGHT_TRANSACTIONS)),
        })
    }

    /// Enqueues `msg` for processing on the next available concurrency slot.
    pub async fn add_message(&self, msg: Message) {
        debug!("adding message {} {}", msg.to.unwrap(), msg.value);
        self.queue.lock().await.push(msg)
    }

    /// Drives the transaction send loop until an unrecoverable error occurs.
    ///
    /// [`run`] subscribes to new block headers and processes messages from the
    /// priority queue. On each new block the nonce is re-synced with the chain.
    /// When a concurrency slot is free, the highest-priority queued message is
    /// popped and processed in a spawned task.
    ///
    /// [`run`]: Self::run
    pub async fn run(&self) -> Result<(), Error> {
        let mut block_stream = self.chain.subscribe_new_blocks().await?;

        loop {
            tokio::select! {
                biased;

                block = block_stream.recv() => {
                    match block {
                        Some(header) => {
                            debug!("Received new block notification {}", header.inner.number);
                            // Every block re-sync our nonce
                            self.nonce_manager.sync_nonce().await?
                        }
                        None => {
                            return Err(Error::ChainError(chain::Error::Subscription(
                                "block stream closed unexpectedly".to_string(),
                            )));
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
            // The permit is moved into the task so it is held for the full
            // duration of process_message, enforcing MAX_IN_FLIGHT_TRANSACTIONS.
            let sender = self.clone();
            tokio::spawn(async move {
                let _permit = permit;
                if let Err(Error::ChainError(chain::Error::Rpc(ErrorResp(e)))) = sender.process_message(msg).await {
                    if e.code == TX_FAILURE_INSUFFICIENT_FUNDS {
                        error!("Insufficient funds to send transaction; dropping message");
                    }
                }
            });
        } else {
            // No messages, release the permit and yield
            drop(permit);
            tokio::task::yield_now().await;
        }
    }

    /// Validates and sends a single message, binding its nonce and gas prices
    /// immediately before submission.
    async fn process_message(&self, msg: Message) -> Result<(), Error> {
        debug!("processing message {} {}", msg.to.unwrap(), msg.value);

        // First ensure the message is still valid
        if msg.is_expired() {
            return Err(Error::MessageExpired);
        }

        // Get the next unscheduled nonce and initial gas prices
        let nonce = self.nonce_manager.get_next_available_nonce().await;
        let (base_fee, priority_fee) = self.gas_manager.get_gas_price(msg.effective_priority()).await?;

        // Send transaction
        self.send_transaction(msg, nonce, base_fee, priority_fee, 0)
            .await?;
        Ok(())
    }

    /// Builds, signs, and broadcasts a transaction, then watches for confirmation.
    ///
    /// On confirmation, this method updates the nonce and gas-price trackers.
    /// On timeout or drop, it delegates to [`handle_transaction_dropped`] which
    /// may bump the fee and retry up to [`MAX_REPLACEMENTS`] times.
    ///
    /// [`handle_transaction_dropped`]: Self::handle_transaction_dropped
    async fn send_transaction(
        &self,
        msg: Message,
        nonce: u64,
        base_fee: u128,
        priority_fee: u128,
        replacement_count: u32,
    ) -> Result<(), Error> {
        Box::pin(async move {
            // Ensure the message is still valid
            if msg.is_expired() {
                self.nonce_manager.mark_nonce_available(nonce).await;
                return Err(Error::MessageExpired);
            }

            // Ensure we haven't exceeded the maximum number of replacements
            if replacement_count > MAX_REPLACEMENTS {
                self.nonce_manager.mark_nonce_available(nonce).await;
                return Err(Error::FeeIncreasesExceeded);
            }

            // Build the transaction
            let tx = TxEip1559 {
                chain_id: self.chain.id(),

                // Message fields
                to: msg.to.map_or(TxKind::Create, TxKind::Call),
                value: msg.value,
                input: msg.data.clone(),

                // Transaction wrapper fields
                nonce,
                gas_limit: msg.gas, // TODO: Sim for gas amount
                max_fee_per_gas: base_fee + priority_fee,
                max_priority_fee_per_gas: priority_fee,

                access_list: Default::default(),
            };

            // Send the transaction and get a watcher
            // If this fails, mark the nonce as available and return the error
            let watcher = match self.chain.send_transaction(tx).await {
                Ok(w) => w,
                Err(e) => {
                    self.nonce_manager.mark_nonce_available(nonce).await;
                    return Err(Error::ChainError(e));
                }
            };

            let tx_hash = *watcher.tx_hash();
            info!("Sent transaction {:?} with nonce {}", tx_hash, nonce);

            // Track the pending transaction
            self.pending.lock().await.insert(tx_hash, PendingTransaction {
                msg,
                nonce,
                priority_fee,
                replacement_count,
                created_at: Instant::now(),
            });

            // Watch the pending transaction for confirmation or timeout
            match self.watch_transaction(watcher).await {
                Ok(_) => {
                    match self.chain.get_receipt(tx_hash).await {
                        // We got a receipt
                        Ok(Some(receipt)) => self.handle_transaction_receipt(tx_hash, receipt).await,

                        // We timed out
                        Ok(None) => self.handle_transaction_dropped(tx_hash).await,

                        // Error getting the receipt
                        Err(e) => {
                            error!("error getting receipt: {}", e);
                            self.handle_transaction_dropped(tx_hash).await
                        }
                    };
                }
                // Transaction dropped
                Err(e) => {
                    error!("error building watcher: {}", e);
                    self.handle_transaction_dropped(tx_hash).await
                }
            };
            Ok(())
        }).await
    }

    /// Registers a confirmation watcher with a [`TX_TIMEOUT`]-second deadline.
    async fn watch_transaction(&self, watcher: PendingTransactionBuilder<Ethereum>) -> Result<TxHash, Error> {
        // Configure the watcher
        let pending = watcher
            .with_required_confirmations(1)
            .with_timeout(Some(Duration::from_secs(TX_TIMEOUT)))
            .register().await?;

        // Wait for the watcher to confirm or timeout
        Ok(pending.await?)
    }

    /// Handles a confirmed transaction - updates the gas and nonce trackers.
    ///
    /// If the receipt indicates a revert, this method delegates to
    /// [`handle_transaction_dropped`] for retry handling.
    ///
    /// [`handle_transaction_dropped`]: Self::handle_transaction_dropped
    async fn handle_transaction_receipt(
        &self,
        tx_hash: B256,
        receipt: TransactionReceipt,
    ) {
        // Handle reverts
        if !receipt.status() {
            // TODO: Handle better
            self.handle_transaction_dropped(tx_hash).await;
            return;
        }

        // Transaction confirmed; remove it from pending and update the gas and nonce trackers
        info!("Transaction {:?} confirmed", tx_hash);
        if let Some(pending_tx) = self.pending.lock().await.remove(&tx_hash) {
            let latency = pending_tx.created_at.elapsed();
            join(
                self.gas_manager.update_on_confirmation(latency, receipt.effective_gas_price),
                self.nonce_manager.update_current_nonce(pending_tx.nonce),
            ).await;
        }
    }

    /// Handles a dropped or timed-out transaction.
    ///
    /// If the message has retries remaining, this method attempts to replace
    /// the transaction with a higher priority fee via [`bump_transaction_fee`].
    /// If retries are exhausted, the message is abandoned and logged.
    ///
    /// [`bump_transaction_fee`]: Self::bump_transaction_fee
    async fn handle_transaction_dropped(&self, tx_hash: B256) {
        let mut pending_tx = {
            let mut pending = self.pending.lock().await;
            match pending.remove(&tx_hash) {
                Some(tx) => tx,
                None => {
                    println!(
                        "Transaction {} not found in in-flight pending map",
                        tx_hash
                    );
                    return;
                }
            }
        }; // lock released here, before any await

        // If we have retries left then attempt to bump the the fee
        if pending_tx.msg.increment_retry() {
            if let Err(e) = self.bump_transaction_fee(tx_hash, pending_tx).await {
                println!("Failed to replace transaction: {:?}", e);
            }
            return;
        }

        // No retries left so log this and abandon
        // TODO: Make requeue to try again later?
        println!(
            "Message for transaction {:?} failed after max retries",
            tx_hash
        );
    }

    /// Re-sends a dropped transaction at the same nonce with a
    /// [`GAS_PRICE_INCREASE_PERCENT`]% higher priority fee.
    async fn bump_transaction_fee(
        &self,
        tx_hash: B256,
        pending_tx: PendingTransaction,
    ) -> Result<(), Error> {
        // Decide new fees
        let base_fee = self.gas_manager.get_base_fee().await;
        let new_priority = bump_by_percent(pending_tx.priority_fee, GAS_PRICE_INCREASE_PERCENT);

        // Remove existing transaction
        self.pending.lock().await.remove(&tx_hash);

        // Send the same msg at the same nonce, with new fees and an incremented replacement_count
        self.send_transaction(
            pending_tx.msg,
            pending_tx.nonce,
            base_fee,
            new_priority,
            pending_tx.replacement_count + 1,
        )
            .await
    }
}

fn bump_by_percent(n: u128, percent: u8) -> u128 {
    n + n * percent as u128 / 100
}

#[cfg(test)]
mod tests {
    use super::{bump_by_percent, Sender};
    use crate::{chain, Message};
    use alloy::primitives::Address;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::mpsc;

    // A minimal mock that satisfies NonceManager::new inside Sender::new.
    fn base_mock() -> chain::MockChainClient {
        let mut mock = chain::MockChainClient::new();
        mock.expect_get_account_nonce()
            .returning(|_| Box::pin(async { Ok(0u64) }));
        mock
    }

    async fn make_sender(mock: chain::MockChainClient) -> Sender {
        Sender::new(Arc::new(mock), Address::default()).await.unwrap()
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
        // Regression: the original formula was `n * percent / 100` (20% OF n),
        // which produced a replacement fee LOWER than the original.
        // Correct formula: `n + n * percent / 100` (n bumped BY percent%).
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
        mock.expect_subscribe_new_blocks().returning(|| {
            // The sender half is dropped immediately, so the receiver is closed.
            let (_tx, rx) = mpsc::channel(1);
            Box::pin(async move { Ok(rx) })
        });

        let sender = make_sender(mock).await;
        let result = sender.run().await;
        assert!(result.is_err(), "run() must return Err when the block stream closes");
    }

    // ── process_next_message() ───────────────────────────────────────────────

    #[tokio::test]
    async fn test_process_next_message_returns_quickly_with_empty_queue() {
        let sender = make_sender(base_mock()).await;
        // With no messages, process_next_message should yield once and return.
        tokio::time::timeout(Duration::from_millis(500), sender.process_next_message())
            .await
            .expect("process_next_message blocked with an empty queue");
    }

    #[tokio::test]
    async fn test_process_next_message_pops_and_attempts_send() {
        let mut mock = base_mock();
        // chain.id() is called to build the TxEip1559.
        mock.expect_id().returning(|| 1337u64);
        // Inject a chain-level failure so we don't need a real provider.
        mock.expect_send_transaction().returning(|_| {
            Box::pin(async { Err(chain::Error::Subscription("injected".into())) })
        });

        let sender = make_sender(mock).await;
        let mut msg = Message::default();
        msg.to = Some(Address::default());
        msg.gas = 21_000;
        sender.add_message(msg).await;
        assert_eq!(sender.queue.lock().await.len(), 1);

        sender.process_next_message().await;
        // Give the spawned task time to run to completion.
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Message was popped regardless of the send outcome.
        assert_eq!(sender.queue.lock().await.len(), 0);
    }
}
