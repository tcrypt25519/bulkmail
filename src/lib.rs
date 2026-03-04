//! Parallel transaction sender with pluggable chain adapters.
//!
//! Bulkmail manages concurrent transaction submission, handling replay
//! protection, fee pricing, retries, and stuck-transaction replacement.
//! The library is generic over a [`ChainAdapter`] — currently Ethereum
//! (EIP-1559) is supported, with Solana planned.
//!
//! ## Usage (Ethereum)
//!
//! 1. Connect to a WebSocket endpoint using [`Chain::new`].
//! 2. Create a [`Sender<Eth>`] via [`Sender::new`].
//! 3. Submit [`Message`] values via [`Sender::add_message`].
//! 4. Drive the send loop with [`Sender::run`].
//!
//! ## Design
//!
//! Nonces and gas prices are **late-bound** — assigned immediately before
//! sending rather than at message creation. This allows messages to be
//! re-queued and replaced without invalidating earlier assignments.
//!
//! Transactions that time out are replaced with a higher priority fee.
//! Messages that exhaust retries are dropped with an error log.
//!
//! [`ChainAdapter`]: adapter::ChainAdapter
//! [`Chain::new`]: Chain::new
//! [`Sender::new`]: Sender::new
//! [`Sender::add_message`]: Sender::add_message
//! [`Sender::run`]: Sender::run
//! [`Sender<Eth>`]: Sender

use alloy::transports::{RpcError, TransportErrorKind};
use thiserror::Error;

pub mod adapter;
pub mod chain;
pub(crate) mod clock;
pub(crate) mod gas_price;
pub mod message;
pub(crate) mod nonce_manager;
mod priority_queue;
pub mod sender;

#[cfg(not(any(feature = "ethereum", feature = "solana")))]
compile_error!("At least one chain feature (\"ethereum\" or \"solana\") must be enabled.");

// Re-export main components for easier use
#[cfg(feature = "ethereum")]
pub use adapter::ethereum::{Eth, EthClient, EthFeeManager, EthReplayProtection, EthRetryStrategy};
#[cfg(feature = "solana")]
pub use adapter::solana::{Sol, SolClient, SolFeeManager, SolReplayProtection, SolRetryStrategy};
pub use chain::{Chain, ChainClient as LegacyChainClient};
pub(crate) use gas_price::GasPriceManager;
pub use message::Message;
pub(crate) use nonce_manager::NonceManager;
pub(crate) use priority_queue::PriorityQueue;
pub use sender::Sender;

/// The top-level error type for the bulkmail library.
#[derive(Error, Debug)]
pub enum Error {
    /// An RPC transport error returned by Alloy.
    #[error("rpc error: {0}")]
    RpcError(#[from] RpcError<TransportErrorKind>),

    /// A transaction signing failure.
    #[error("signing error")]
    SigningError(#[from] alloy::signers::Error),

    /// An error waiting for a pending transaction to confirm.
    #[error("pending transaction error: {0}")]
    PendingTransaction(#[from] alloy::providers::PendingTransactionError),

    /// The message specified more dependencies than the system allows.
    #[error("Too many dependencies: {0}")]
    TooManyDependencies(usize),

    /// Gas price computation produced an invalid result.
    #[error("Gas price error: {0}")]
    GasPriceError(String),

    /// The message's deadline passed before it could be sent.
    #[error("Message expired")]
    MessageExpired,

    /// The message was retried the maximum number of times without confirming.
    #[error("Retries exceeded")]
    RetriesExceeded,

    /// The stuck transaction was replaced the maximum number of times without confirming.
    #[error("Fee increases exceeded")]
    FeeIncreasesExceeded,

    /// The computed gas price was below the network minimum.
    #[error("Gas price too low")]
    GasPriceTooLow,

    /// A pre-send simulation of the transaction failed.
    #[error("Simulation failed")]
    SimulationFailed,

    /// An error originating from [`Chain`] operations.
    ///
    /// [`Chain`]: chain::Chain
    #[error("chain error: {0}")]
    ChainError(#[from] chain::Error),

    /// A transaction was dropped or timed out without confirming.
    #[error("transaction dropped: {0}")]
    TransactionDropped(alloy::primitives::B256),

    /// A transaction was confirmed but reverted on-chain.
    #[error("transaction reverted: {0}")]
    TransactionReverted(alloy::primitives::B256),

    /// A subscription was closed unexpectedly.
    #[error("subscription closed")]
    SubscriptionClosed,

    /// An error originating from Solana RPC or Pubsub operations.
    #[error("solana error: {0}")]
    SolanaError(String),
}
