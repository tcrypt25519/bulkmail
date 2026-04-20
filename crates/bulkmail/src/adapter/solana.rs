//! Solana adapter — TODO: implement Solana support behind the `solana` feature.
//!
//! [`Sol`] is the zero-sized tag type. Instantiate a [`Sender<Sol>`] once the
//! Solana adapter is implemented.
//!
//! [`Sender<Sol>`]: crate::Sender

use crate::{
    Error,
    adapter::{
        BlockReceiver, ChainAdapter, ChainClient, FeeManager, PendingTransaction, ReplayProtection,
        RetryDecision, RetryStrategy, SendOutcome, TransactionStatus,
    },
};
use async_trait::async_trait;
use solana_client::{nonblocking::rpc_client::RpcClient, pubsub_client::PubsubClient};
use solana_sdk::{
    commitment_config::CommitmentConfig, hash::Hash, instruction::Instruction,
    signature::Signature, transaction::VersionedTransaction,
};
use solana_transaction_status::TransactionConfirmationStatus;
use std::{sync::Arc, time::Duration};
use tokio::sync::Mutex;

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
    rpc: Arc<RpcClient>,
    pubsub_url: String,
}

impl SolClient {
    #[allow(dead_code)]
    pub fn new(rpc: Arc<RpcClient>, pubsub_url: impl Into<String>) -> Self {
        Self {
            rpc,
            pubsub_url: pubsub_url.into(),
        }
    }
}

#[async_trait]
impl ChainClient<Sol> for SolClient {
    async fn subscribe_new_blocks(&self) -> Result<BlockReceiver, Error> {
        let (subscription, receiver) = PubsubClient::slot_subscribe(&self.pubsub_url)
            .map_err(|err| Error::SolanaError(format!("slot_subscribe failed: {err}")))?;

        let (tx, rx) = tokio::sync::mpsc::channel(32);
        std::thread::spawn(move || {
            // Keep the subscription alive for the lifetime of this thread.
            let _subscription = subscription;
            for slot_info in receiver.iter() {
                if tx.blocking_send(slot_info.slot).is_err() {
                    break;
                }
            }
        });

        Ok(rx)
    }

    async fn get_block_number(&self) -> Result<u64, Error> {
        self.rpc
            .get_slot()
            .await
            .map_err(|err| Error::SolanaError(format!("get_slot failed: {err}")))
    }

    async fn send_transaction(
        &self,
        _msg: &crate::Message,
        _fee: &SolFeeParams,
        _replay_token: &Hash,
    ) -> Result<SendOutcome<Sol>, Error> {
        Err(Error::SolanaError(
            "send_transaction not implemented for Solana yet".to_string(),
        ))
    }

    async fn get_transaction_status(&self, _id: &Signature) -> Result<TransactionStatus, Error> {
        let status = self
            .rpc
            .get_signature_statuses(&[*_id])
            .await
            .map_err(|err| Error::SolanaError(format!("get_signature_statuses failed: {err}")))?
            .value
            .into_iter()
            .next()
            .flatten();

        let status = match status {
            None => return Ok(TransactionStatus::Pending),
            Some(status) => status,
        };

        if let Some(err) = status.err {
            return Ok(TransactionStatus::Failed {
                reason: format!("{err:?}"),
            });
        }

        match status.confirmation_status() {
            TransactionConfirmationStatus::Finalized => Ok(TransactionStatus::Finalized {
                number: status.slot,
            }),
            TransactionConfirmationStatus::Confirmed | TransactionConfirmationStatus::Processed => {
                Ok(TransactionStatus::Confirmed {
                    number: status.slot,
                })
            }
        }
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
pub struct SolReplayProtection {
    rpc: Arc<RpcClient>,
    state: Mutex<BlockhashState>,
}

#[derive(Clone, Copy, Debug)]
struct BlockhashState {
    hash: Hash,
    last_valid_block_height: u64,
}

const BLOCKHASH_REFRESH_MARGIN: u64 = 30;

impl SolReplayProtection {
    pub async fn new(rpc: Arc<RpcClient>) -> Result<Self, Error> {
        let (hash, last_valid_block_height) = rpc
            .get_latest_blockhash_with_commitment(CommitmentConfig::processed())
            .await
            .map_err(|err| Error::SolanaError(format!("get_latest_blockhash failed: {err}")))?;

        Ok(Self {
            rpc,
            state: Mutex::new(BlockhashState {
                hash,
                last_valid_block_height,
            }),
        })
    }
}

#[async_trait]
impl ReplayProtection<Sol> for SolReplayProtection {
    async fn next(&self) -> Hash {
        let state = self.state.lock().await;
        state.hash
    }

    async fn sync(&self) -> Result<(), Error> {
        let current_height = self
            .rpc
            .get_block_height()
            .await
            .map_err(|err| Error::SolanaError(format!("get_block_height failed: {err}")))?;

        let should_refresh = {
            let state = self.state.lock().await;
            current_height + BLOCKHASH_REFRESH_MARGIN >= state.last_valid_block_height
        };

        if should_refresh {
            let (hash, last_valid_block_height) = self
                .rpc
                .get_latest_blockhash_with_commitment(CommitmentConfig::processed())
                .await
                .map_err(|err| Error::SolanaError(format!("get_latest_blockhash failed: {err}")))?;

            let mut state = self.state.lock().await;
            state.hash = hash;
            state.last_valid_block_height = last_valid_block_height;
        }

        Ok(())
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
fn build_transaction(_instructions: Vec<Instruction>, _blockhash: Hash) -> VersionedTransaction {
    todo!("Construct VersionedTransaction with payer + instructions");
}
