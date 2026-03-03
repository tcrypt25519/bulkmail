//! Chain-agnostic adapter traits and types.
//!
//! The [`ChainAdapter`] trait binds all chain-specific components together via
//! associated types. [`Sender`](crate::Sender) is generic over `A: ChainAdapter`,
//! so all chain operations are statically dispatched through the adapter.
//!
//! Each chain provides a zero-sized tag type (e.g., [`ethereum::Eth`]) that
//! implements [`ChainAdapter`] and locks all associated types together at the
//! type level.

pub mod ethereum;

use crate::Error;
use async_trait::async_trait;
use std::{fmt::Debug, time::Duration};
use tokio::sync::mpsc;

/// A channel receiver that yields new block/slot notifications.
///
/// On Ethereum this carries block headers (bridged to a `u64` block number).
/// On Solana this carries slot notifications.
pub type BlockNotification = u64;
pub type BlockReceiver = mpsc::Receiver<BlockNotification>;

/// The root abstraction that binds all chain-specific components together.
///
/// Each chain (Ethereum, Solana) provides a zero-sized tag type that
/// implements this trait and specifies its associated types. `Sender<A>`
/// is generic over this trait, so all chain components are statically
/// bound and monomorphized at compile time.
pub trait ChainAdapter: Send + Sync + Sized + 'static {
    /// Chain-specific fee parameters.
    type FeeParams: Send + Sync + Clone + Debug;

    /// Chain-specific replay-protection token (nonce or blockhash).
    type ReplayToken: Send + Sync + Clone + Debug;

    /// Opaque transaction identifier returned after sending.
    type TxId: Send + Sync + Clone + Debug + Eq + std::hash::Hash;

    /// The chain connection / RPC client.
    type Client: ChainClient<Self> + Send + Sync;

    /// Fee estimation and adaptation.
    type FeeManager: FeeManager<Self> + Send + Sync;

    /// Replay protection (nonce management or blockhash caching).
    type ReplayProtection: ReplayProtection<Self> + Send + Sync;

    /// Transaction retry / fee-bump strategy.
    type RetryStrategy: RetryStrategy<Self> + Send + Sync;
}

/// Chain-agnostic interface for submitting transactions and monitoring blocks/slots.
#[async_trait]
pub trait ChainClient<A: ChainAdapter>: Send + Sync {
    /// Subscribe to new block/slot notifications.
    async fn subscribe_new_blocks(&self) -> Result<BlockReceiver, Error>;

    /// Get the current block number (ETH) or confirmed block height (SOL).
    async fn get_block_number(&self) -> Result<u64, Error>;

    /// Build, sign, submit, and watch a transaction to completion.
    ///
    /// Returns a [`SendOutcome`] indicating whether the transaction confirmed,
    /// was dropped/timed out, or failed on-chain. This allows the caller to
    /// handle each outcome generically without Ethereum-specific error types.
    async fn send_transaction(
        &self,
        msg: &crate::Message,
        fee: &A::FeeParams,
        replay_token: &A::ReplayToken,
    ) -> Result<SendOutcome<A>, Error>;

    /// Check the confirmation status of a previously sent transaction.
    async fn get_transaction_status(&self, id: &A::TxId) -> Result<TransactionStatus, Error>;
}

/// The outcome of a send-and-watch operation.
#[derive(Debug)]
pub enum SendOutcome<A: ChainAdapter> {
    /// Transaction was confirmed on-chain.
    Confirmed { tx_id: A::TxId },
    /// Transaction was dropped or timed out without confirming.
    Dropped { tx_id: A::TxId },
    /// Transaction was confirmed but reverted on-chain.
    Reverted { tx_id: A::TxId },
}

/// Chain-agnostic transaction confirmation status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransactionStatus {
    /// Transaction is pending / not yet confirmed.
    Pending,
    /// Transaction has been confirmed.
    /// On Ethereum: the block number. On Solana: the slot number.
    Confirmed { number: u64 },
    /// Transaction has been finalized (irreversible).
    Finalized { number: u64 },
    /// Transaction failed on-chain.
    Failed { reason: String },
    /// Transaction expired (e.g., blockhash expired on Solana).
    Expired,
}

/// Chain-agnostic fee computation.
#[async_trait]
pub trait FeeManager<A: ChainAdapter>: Send + Sync {
    /// Compute fee parameters for a transaction with the given message priority.
    async fn get_fee_params(&self, priority: u32) -> Result<A::FeeParams, Error>;

    /// Record a confirmed transaction's latency and fee for adaptation.
    async fn update_on_confirmation(&self, confirmation_time: Duration, fee_paid: &A::FeeParams);

    /// Bump fee params for a retry (e.g., +20%).
    fn bump_fee(&self, current: &A::FeeParams) -> A::FeeParams;

    /// Return the current base fee estimate.
    async fn get_base_fee(&self) -> A::FeeParams;
}

/// Chain-agnostic replay protection (nonce management or blockhash caching).
///
/// `next()` is synchronous — it reads from in-memory state only. RPC calls
/// to refresh state happen in `sync()`, called on each new block/slot.
#[async_trait]
pub trait ReplayProtection<A: ChainAdapter>: Send + Sync {
    /// Get a replay-protection token for a new transaction.
    /// Must be serviced from in-memory state — no RPC calls.
    async fn next(&self) -> A::ReplayToken;

    /// Sync with the chain (called on each new block/slot notification).
    /// This is where RPC calls to refresh state happen.
    async fn sync(&self) -> Result<(), Error>;
}

/// Chain-agnostic retry / fee-bump strategy.
///
/// Each chain implements its own retry mechanics. Ethereum replaces at the
/// same nonce with bumped fees; Solana resubmits with a fresh blockhash.
#[async_trait]
pub trait RetryStrategy<A: ChainAdapter>: Send + Sync {
    /// Handle a transaction that was dropped or timed out.
    ///
    /// The strategy decides whether to bump fees and resubmit,
    /// re-queue the message, or abandon it.
    async fn handle_dropped(
        &self,
        pending: &PendingTransaction<A>,
        client: &A::Client,
        fees: &A::FeeManager,
        replay: &A::ReplayProtection,
    ) -> RetryDecision<A>;

    /// Handle a confirmed transaction — update internal tracking.
    async fn handle_confirmed(
        &self,
        pending: &PendingTransaction<A>,
        fees: &A::FeeManager,
        replay: &A::ReplayProtection,
        confirmation_time: Duration,
    );
}

/// The decision returned by a retry strategy.
pub enum RetryDecision<A: ChainAdapter> {
    /// Resubmit with new fee params and replay token.
    Resubmit {
        fee: A::FeeParams,
        replay_token: A::ReplayToken,
    },
    /// Re-queue the message for a fresh attempt later.
    Requeue,
    /// Abandon the message.
    Abandon,
}

/// A transaction that has been sent and is pending confirmation.
pub struct PendingTransaction<A: ChainAdapter> {
    pub msg: crate::Message,
    pub tx_id: A::TxId,
    pub fee: A::FeeParams,
    pub replay_token: A::ReplayToken,
    pub created_at: std::time::Instant,
    pub replacement_count: u32,
}

impl<A: ChainAdapter> Clone for PendingTransaction<A> {
    fn clone(&self) -> Self {
        Self {
            msg: self.msg.clone(),
            tx_id: self.tx_id.clone(),
            fee: self.fee.clone(),
            replay_token: self.replay_token.clone(),
            created_at: self.created_at,
            replacement_count: self.replacement_count,
        }
    }
}
