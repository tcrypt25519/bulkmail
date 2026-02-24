//! Parallel EIP-1559 transaction sender for Ethereum.
//!
//! Bulkmail manages concurrent transaction submission to a fast EVM chain,
//! handling nonce sequencing, gas pricing, retries, and stuck-transaction
//! replacement.
//!
//! ## Usage
//!
//! 1. Connect to a WebSocket endpoint using [`Chain::new`].
//! 2. Create a [`Sender`] bound to a signing address via [`Sender::new`].
//! 3. Submit [`Message`] values via [`Sender::add_message`].
//! 4. Drive the send loop with [`Sender::run`].
//!
//! ## Design
//!
//! Nonces and gas prices are **late-bound** - assigned immediately before
//! sending rather than at message creation. This allows messages to be
//! re-queued and replaced without invalidating earlier assignments.
//!
//! Up to 16 transactions may be in-flight concurrently. Transactions that
//! time out are replaced with a 20% higher priority fee, up to 3 times.
//! Messages that exhaust retries are dropped with an error log.
//!
//! [`Chain::new`]: Chain::new
//! [`Sender::new`]: Sender::new
//! [`Sender::add_message`]: Sender::add_message
//! [`Sender::run`]: Sender::run

use alloy::transports::{RpcError, TransportErrorKind};
use thiserror::Error;

pub mod message;
pub mod sender;
pub mod chain;
mod gas_price;
mod priority_queue;
mod nonce_manager;

// Re-export main components for easier use
pub use chain::{BlockReceiver, Chain, ChainClient};
pub use sender::Sender;
pub use message::Message;
pub(crate) use gas_price::GasPriceManager;
pub(crate) use nonce_manager::NonceManager;
pub(crate) use priority_queue::PriorityQueue;

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
}
