//! Solana adapter — TODO: implement Solana support behind the `solana` feature.
//!
//! [`Sol`] is the zero-sized tag type. Instantiate a [`Sender<Sol>`] once the
//! Solana adapter is implemented.
//!
//! [`Sender<Sol>`]: crate::Sender

use crate::adapter::{
    BlockReceiver, ChainAdapter, ChainClient, FeeManager, PendingTransaction, ReplayProtection,
    RetryDecision, RetryStrategy, SendOutcome, TransactionStatus,
};
use crate::Error;
use async_trait::async_trait;
use solana_client::{pubsub_client::PubsubClient, rpc_client::RpcClient};
use solana_sdk::{
    hash::Hash, instruction::Instruction, signature::Signature, transaction::VersionedTransaction,
};
use std::sync::Arc;
use std::time::Duration;

/// Solana fee parameters (placeholder).
#[derive(Debug, Clone)]
pub struct SolFeeParams {
    /// Compute unit price in micro-lamports.
    pub compute_unit_price: u64,
    /// Compute unit limit.
    pub compute_unit_limit: u32,
}

/// Solana adapter tag type. All SOL-specific types are associated here.
pub struct Sol;

impl ChainAdapter for Sol {
    type FeeParams = SolFeeParams;
    type ReplayToken = Hash; // recent blockhash
    type TxId = Signature; // transaction signature
    type Client = SolClient;
    type FeeManager = SolFeeManager;
    type ReplayProtection = SolReplayProtection;
    type RetryStrategy = SolRetryStrategy;
}

/// Solana chain client (RPC + Pubsub).
pub struct SolClient {
    #[allow(dead_code)]
    rpc: Arc<RpcClient>,
    #[allow(dead_code)]
    pubsub: Arc<PubsubClient>,
}

impl SolClient {
    #[allow(dead_code)]
    pub fn new(rpc: Arc<RpcClient>, pubsub: Arc<PubsubClient>) -> Self {
        Self { rpc, pubsub }
    }
}

#[async_trait]
impl ChainClient<Sol> for SolClient {
    async fn subscribe_new_blocks(&self) -> Result<BlockReceiver, Error> {
        todo!("Implement slot subscription via PubsubClient");
    }

    async fn get_block_number(&self) -> Result<u64, Error> {
        todo!("Implement get_latest_blockhash / block height via RpcClient");
    }

    async fn send_transaction(
        &self,
        _msg: &crate::Message,
        _fee: &SolFeeParams,
        _replay_token: &Hash,
    ) -> Result<SendOutcome<Sol>, Error> {
        todo!("Implement send + confirm using VersionedTransaction");
    }

    async fn get_transaction_status(&self, _id: &Signature) -> Result<TransactionStatus, Error> {
        todo!("Implement signature status lookup");
    }
}

/// Solana fee manager (placeholder).
pub struct SolFeeManager;

#[async_trait]
impl FeeManager<Sol> for SolFeeManager {
    async fn get_fee_params(&self, _priority: u32) -> Result<SolFeeParams, Error> {
        todo!("Use getRecentPrioritizationFees to derive CU price/limit");
    }

    async fn update_on_confirmation(&self, _confirmation_time: Duration, _fee_paid: &SolFeeParams) {
        todo!("Update internal fee model from confirmation latency");
    }

    fn bump_fee(&self, current: &SolFeeParams) -> SolFeeParams {
        SolFeeParams {
            compute_unit_price: current.compute_unit_price.saturating_add(1),
            compute_unit_limit: current.compute_unit_limit,
        }
    }

    async fn get_base_fee(&self) -> SolFeeParams {
        todo!("Return baseline fee params");
    }
}

/// Solana replay protection (blockhash cache placeholder).
pub struct SolReplayProtection;

#[async_trait]
impl ReplayProtection<Sol> for SolReplayProtection {
    async fn next(&self) -> Hash {
        todo!("Return most recent cached blockhash");
    }

    async fn sync(&self) -> Result<(), Error> {
        todo!("Refresh cached blockhash / lastValidBlockHeight");
    }
}

/// Solana retry strategy (placeholder).
pub struct SolRetryStrategy;

#[async_trait]
impl RetryStrategy<Sol> for SolRetryStrategy {
    async fn handle_dropped(
        &self,
        _pending: &PendingTransaction<Sol>,
        _client: &SolClient,
        _fees: &SolFeeManager,
        _replay: &SolReplayProtection,
    ) -> RetryDecision<Sol> {
        todo!("Resubmit with fresh blockhash + bumped CU price");
    }

    async fn handle_confirmed(
        &self,
        _pending: &PendingTransaction<Sol>,
        _fees: &SolFeeManager,
        _replay: &SolReplayProtection,
        _confirmation_time: Duration,
    ) {
        todo!("Update fee model / replay protection state");
    }
}

// --- Solana payload helpers (placeholder) -----------------------------------

#[allow(dead_code)]
fn build_transaction(
    _instructions: Vec<Instruction>,
    _blockhash: Hash,
) -> VersionedTransaction {
    todo!("Construct VersionedTransaction with payer + instructions");
}
